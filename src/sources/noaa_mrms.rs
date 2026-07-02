// NOAA MRMS (Multi-Radar Multi-Sensor) radar-QPE weather source.
//
// MRMS is the US national gauge-corrected radar rain grid: a 1 km CONUS grid
// refreshed about every 2 minutes, published by NOAA as GRIB2. It is the best
// off-yard read of whether rain actually FELL on a location, short of the user's
// own gauge, because it sees the cell over the user's block rather than the
// hourly report from a distant airport station (the NWS observation case).
//
// TWO PRODUCTS PER CYCLE. The adapter pulls TWO MRMS grids each poll so it
// surfaces both halves of the rain picture, each with its OWN valid time so the
// merge tracks each field's freshness independently:
//   * RATE (PrecipRate): the instantaneous radar rain RATE in mm/hr, updated
//     about every 2 minutes and valid ~now. It maps to RainIntensityInHr and is
//     the FRESH "is it raining right now" read. We reject it if its grid valid
//     time is older than `RATE_MAX_STALENESS` (~45 min): a product refreshed
//     every couple minutes should never be that old, so a stale one means the
//     radar feed is lagging and we drop it rather than hand the merge a fresh
//     freshness window on an old reading.
//   * ACCUMULATION (MultiSensor_QPE_01H_Pass2): the gauge-corrected 1 hour
//     rainfall total in mm. This is the ACCURATE read of how much rain fell, but
//     pass-2 publishes ~80 min late so its valid time is inherently ~1 to 1.5 hr
//     behind. It maps to RainTodayIn and we reject it only past
//     `ACCUM_MAX_STALENESS` (~3 hr): a gauge-corrected hourly product is
//     expected to lag and is still decision-useful at 1 to 1.5 hr.
// Each product emits a SEPARATE SourceEvent::Observation stamped with that
// grid's own valid time, so the fresh rate and the lagged accumulation never
// share a single timestamp and the merge ages each field on its own clock.
//
// Keyless + US-only: no API key, no account; the MRMS grids are public NOAA
// data. The adapter is auto-seeded in the US exactly like NWS (see
// `config::region`), at a priority ABOVE NWS and below a real on-LAN gauge.
//
// HONESTY: MRMS rain is `RadarQpe` (observation-grade radar QPE), NOT a model
// forecast. It emits ONLY the current rain scalars (RainIntensityInHr +
// RainTodayIn) into the merge; it is NOT a forecast-snapshot provider, so it is
// excluded from `SourceKind::is_forecast()` and never feeds the forecast bridge.
//
// DATA PATH (confirmed June 2026):
//   GET https://mrms.ncep.noaa.gov/2D/{product}/MRMS_{product}.latest.grib2.gz
// The `.latest.grib2.gz` symlink always points at the newest publish, so we
// never do date/time arithmetic to find the current file. It is a gzip-wrapped
// single-message GRIB2 (about 0.5 MB on the wire). We gunzip it in memory,
// decode the one message with `gribberish`, and index the single grid cell
// over the deployment lat/lon. We do NOT keep the full decoded CONUS field
// (about 24.5M f32 cells); `gribberish` decodes the whole message to a Vec, but
// we read exactly one cell out of it and drop the rest each cycle. There is no
// server-side spatial subset on the NCEP HTTP directory (the NOMADS grib-filter
// covers model output, not the MRMS 2D grids), so a small full-grid pull plus a
// single-cell index is the smallest reliable path. The download is ~0.5 MB and
// the poll cadence is slow (see POLL_INTERVAL), so this is cheap.
//
// GRID: MRMS is a regular 0.01 deg lon/lat (Plate Caree) grid. The cell over a
// point is pure arithmetic off the GRIB2 grid-definition start lat/lon + step
// (no projection), so we read the message's own grid metadata
// (`grid_dimensions` + `latlng_projector`) rather than hardcoding the CONUS
// corners; that keeps us correct if NOAA ever reshapes the grid. NCEP MRMS
// longitudes are published on the 0..360 convention (about 230.005..299.995),
// so we normalize the deployment's signed longitude into 0..360 before indexing.
//
// UNITS + MISSING: the QPE products are mm of accumulation; PrecipRate is mm/hr.
// We convert mm -> inches through the shared `units::to_canonical` seam (so the
// conversion stays single-sourced). MRMS uses NEGATIVE fill flags for "no data"
// (-3 = outside any radar umbrella / no coverage, -999 = missing); we SKIP any
// negative or non-finite cell rather than emit it as 0, so a no-coverage cell
// never reads as a confirmed dry 0 that would green-light irrigation.
//
// ssr-only: this whole `sources` module tree is `#[cfg(feature = "ssr")]` at the
// lib root, so neither this adapter nor the `gribberish` GRIB decoder is ever
// pulled into the hydrate/wasm bundle, even though the wasm build still sees the
// `SourceKind::NoaaMrms` variant in the shared config schema.

use std::collections::HashSet;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::schema::{Location, NoaaMrmsConfig};
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};

/// NCEP public HTTP directory that fronts the live MRMS 2D grids. Each product
/// lives at `{MRMS_BASE}/{product}/` with a `.latest.grib2.gz` symlink.
const MRMS_BASE: &str = "https://mrms.ncep.noaa.gov/2D";

/// MRMS PrecipRate publishes every ~2 minutes. Poll every 3 minutes so the live
/// radar rate stays genuinely current (and keeps reading fresh, the health badge
/// greens out under ~5 min) without hammering NCEP. The gauge-corrected hourly
/// accumulation barely moves between polls, so re-fetching it on the same cadence
/// is cheap waste, not a correctness issue.
const POLL_INTERVAL: Duration = Duration::from_secs(3 * 60);

/// Bounded retry budget for a single MRMS product fetch. NCEP intermittently
/// drops the response body mid-transfer (reqwest "error decoding response
/// body"), which is transient; up to 2 retries (3 total attempts) lets a single
/// dropped body recover within the same poll cycle instead of failing the cycle
/// and flipping the source unreachable. Small + bounded so a genuinely-down NCEP
/// still fails promptly (3 attempts, well inside the 30s per-request timeout +
/// the 3-min poll cadence) and the caller keeps its emit-nothing behavior.
const FETCH_MAX_RETRIES: u32 = 2;

/// Backoff between MRMS fetch retries. Short (the failure is a dropped body, not
/// rate-limiting), so a recovery costs sub-second latency; 3 attempts at 250 ms
/// add at most ~0.5s, negligible against the 3-min poll cadence.
const FETCH_RETRY_BACKOFF: Duration = Duration::from_millis(250);

/// MRMS no-data / no-coverage fill threshold. MRMS encodes "outside any radar
/// umbrella" as -3 and "missing" as -999 (and other negative flags), so ANY
/// negative cell is treated as no-reading. A valid QPE / precip rate is >= 0.
/// Skipping (rather than zeroing) keeps a no-coverage cell from reading as a
/// confirmed dry 0 that would falsely green-light irrigation.
const MRMS_MIN_VALID: f64 = 0.0;

/// Maximum age of the RATE grid (PrecipRate) we will still emit. PrecipRate is
/// instantaneous and refreshed about every 2 minutes, so a fresh product should
/// never be more than a few minutes old; 45 minutes is a wide margin that still
/// rejects a clearly lagging radar feed. We stamp the observation with the
/// GRIB's own VALID time (not Utc::now), so a stale grid that slips the
/// `.latest` symlink never reads as fresh to the freshness gate downstream.
const RATE_MAX_STALENESS: Duration = Duration::from_secs(45 * 60);

/// Maximum age of the ACCUMULATION grid (the gauge-corrected hourly QPE) we will
/// still emit. Pass-2 publishes ~80 min late, so an hourly gauge-corrected grid
/// is INHERENTLY ~1 to 1.5 hr behind; that lag is expected and the reading is
/// still decision-useful, so the accumulation tolerates a far wider window than
/// the instantaneous rate. 3 hours still rejects a clearly stuck publish while
/// never dropping a normally-lagged hourly grid.
const ACCUM_MAX_STALENESS: Duration = Duration::from_secs(3 * 60 * 60);

/// Decoded grid geometry needed to index the cell over a point: the row-major
/// `ny x nx` shape (latitude-major, longitude fastest, GRIB scan order) plus the
/// start coordinate and per-cell step of each axis. Pulled from the GRIB2 grid
/// definition so we never hardcode the CONUS corners.
#[derive(Debug, Clone, Copy)]
struct GridGeometry {
    /// Rows (latitude count).
    ny: usize,
    /// Columns (longitude count).
    nx: usize,
    /// Latitude of the first grid row (GRIB La1).
    lat_start: f64,
    /// Longitude of the first grid column (GRIB Lo1), on the 0..360 convention.
    lon_start: f64,
    /// Signed latitude step per row (negative when the grid scans north -> south,
    /// which MRMS does: La1 about 54.995 down to about 20.005).
    dlat: f64,
    /// Signed longitude step per column (positive for MRMS: west -> east).
    dlon: f64,
}

/// Which canonical rain field a product's cell value maps to, and how to read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RainKind {
    /// An accumulation product (the QPE grids, e.g. MultiSensor_QPE_01H_Pass2):
    /// the cell is mm of liquid accumulated over the product's window. A 1 hour
    /// accumulation is, over that hour, an in/hr rate, so it emits BOTH the
    /// today-accumulation field and the intensity field (mirrors how the NWS
    /// adapter treats precipitationLastHour). Longer windows still emit the
    /// accumulation; the intensity reading is only meaningful for the 1 hour
    /// window, so we gate it on `accum_is_hourly`.
    Accumulation { accum_is_hourly: bool },
    /// An instantaneous-rate product (PrecipRate): the cell is mm/hr, which maps
    /// straight to RainIntensityInHr and carries no accumulation total.
    Rate,
}

/// Classify a configured MRMS product string into how its cell value should be
/// read. The QPE family is accumulation (mm); PrecipRate is an instantaneous
/// rate (mm/hr). Defaults to a 1 hour accumulation for the gauge-corrected
/// pass-2 product (our default) and for any unrecognized QPE-shaped name.
fn classify_product(product: &str) -> RainKind {
    let p = product.to_ascii_lowercase();
    if p.contains("preciprate") {
        RainKind::Rate
    } else {
        // Any "..._QPE_<NN>H_..." accumulation. "01H" is hourly; anything else
        // (03H/06H/12H/24H, or a windowless name) emits only the accumulation.
        let accum_is_hourly = p.contains("_qpe_01h") || !p.contains("_qpe_");
        RainKind::Accumulation { accum_is_hourly }
    }
}

impl RainKind {
    /// The per-product staleness window: a grid whose own valid time is older
    /// than this is dropped (emit nothing) rather than handed to the merge. The
    /// instantaneous RATE refreshes every couple minutes so it tolerates only a
    /// short lag (`RATE_MAX_STALENESS`); the gauge-corrected hourly
    /// ACCUMULATION publishes ~80 min late by design and is still useful at ~1
    /// to 1.5 hr, so it tolerates a far wider window (`ACCUM_MAX_STALENESS`).
    fn max_staleness(self) -> Duration {
        match self {
            RainKind::Rate => RATE_MAX_STALENESS,
            RainKind::Accumulation { .. } => ACCUM_MAX_STALENESS,
        }
    }
}

pub struct NoaaMrms {
    id: String,
    config: NoaaMrmsConfig,
    location: Location,
    client: Client,
}

impl NoaaMrms {
    pub fn new(id: impl Into<String>, config: NoaaMrmsConfig, location: Location) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("LocalSky (https://localsky.io)")
            .build()
            .expect("reqwest client construction");
        Self {
            id: id.into(),
            config,
            location,
            client,
        }
    }

    /// The accumulation product id (`config.product`), the gauge-corrected
    /// hourly QPE total grid.
    fn accum_product(&self) -> &str {
        &self.config.product
    }

    /// The rate product id (`config.rate_product`), the instantaneous PrecipRate
    /// grid.
    fn rate_product(&self) -> &str {
        &self.config.rate_product
    }

    /// URL of the always-current GRIB2 for an arbitrary MRMS product.
    fn latest_url(&self, product: &str) -> String {
        format!("{MRMS_BASE}/{product}/MRMS_{product}.latest.grib2.gz")
    }

    /// Download the latest gzipped GRIB2 for `product` and gunzip it into raw
    /// GRIB2 bytes.
    ///
    /// BOUNDED RETRY (robustness fix): the PrecipRate fetch intermittently fails
    /// with reqwest "error decoding response body" when NCEP drops the body
    /// mid-transfer. That single dropped body should not fail the whole poll
    /// cycle (which would flip the source unreachable and stop the live rate),
    /// so the GET + body read is retried up to `FETCH_MAX_RETRIES` times with a
    /// short backoff. Only the transient network/decode path is retried; an
    /// `error_for_status` HTTP error (a real 4xx/5xx) is treated the same as a
    /// success-shaped transient here (retried then surfaced) so a persistently
    /// bad product still fails after the bounded attempts and the caller keeps
    /// its existing emit-nothing behavior. The `.latest` symlink is idempotent,
    /// so a retry simply re-pulls the current grid.
    async fn fetch_grib(&self, product: &str) -> anyhow::Result<Vec<u8>> {
        let url = self.latest_url(product);
        let gz = self.fetch_gz_with_retry(&url, product).await?;
        gunzip(&gz)
    }

    /// GET `url` with the bounded transient-retry policy, returning the raw
    /// (still-gzipped) body. Factored out of `fetch_grib` (which then gunzips) so
    /// the retry behavior is unit-testable against a local server without the
    /// `MRMS_BASE`-derived production URL or the gunzip step. `product` is only
    /// used for log context.
    async fn fetch_gz_with_retry(&self, url: &str, product: &str) -> anyhow::Result<Vec<u8>> {
        let mut attempt = 0u32;
        loop {
            match self.fetch_grib_once(url).await {
                Ok(gz) => return Ok(gz),
                Err(e) => {
                    if attempt >= FETCH_MAX_RETRIES {
                        return Err(e);
                    }
                    warn!(
                        source_id = %self.id,
                        product = %product,
                        attempt = attempt + 1,
                        error = %e,
                        "MRMS GRIB fetch transient failure; retrying after backoff"
                    );
                    tokio::time::sleep(FETCH_RETRY_BACKOFF).await;
                    attempt += 1;
                }
            }
        }
    }

    /// A single GET + body read for `url`, returning the gzipped bytes. Split out
    /// of `fetch_grib` so the retry loop can re-issue exactly the transient step
    /// (the GET and the `.bytes()` body read, where the "error decoding response
    /// body" drop surfaces) without re-deriving the URL each attempt. Returns an
    /// owned `Vec<u8>` so the retry loop holds no reqwest type across the await.
    async fn fetch_grib_once(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        let gz = self
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        Ok(gz.to_vec())
    }

    /// Read the grid cell over the deployment lat/lon out of a decoded GRIB2 for
    /// the given `product` and turn it into canonical rain fields stamped with
    /// the GRIB's own VALID time. The product is classified into a [`RainKind`]
    /// (PrecipRate -> Rate, QPE -> Accumulation), which selects BOTH the field
    /// mapping AND the per-product staleness window (the fresh rate tolerates a
    /// short lag, the lagged hourly accumulation a wide one).
    ///
    /// Returns `None` (emit nothing) on a no-coverage / missing cell (skip,
    /// never a false 0), on any decode problem (logged), or on a grid whose
    /// valid time is older than that product's staleness window (logged), so the
    /// caller never emits a fabricated or stale-looking reading. On a real
    /// reading it returns the fields plus the valid epoch to stamp.
    fn fields_from_grib(
        &self,
        product: &str,
        grib: &[u8],
        now_epoch: i64,
    ) -> Option<DecodedObservation> {
        let kind = classify_product(product);
        let cell = match decode_point_value(grib, self.location.lat, self.location.lon) {
            Ok(c) => c,
            Err(e) => {
                warn!(source_id = %self.id, product = %product, error = %e, "MRMS GRIB decode failed");
                return None;
            }
        };

        // Reject a stuck publish: the `.latest` symlink can keep serving the same
        // grid for hours, so we trust the GRIB's OWN valid time, not Utc::now().
        // A grid older than this PRODUCT's staleness window is dropped (not
        // emitted) so a stale grid never reads as fresh to the freshness gate
        // downstream. The rate gets the short window, the accumulation the wide
        // one (it is inherently ~1 to 1.5 hr behind and still useful).
        let age = now_epoch - cell.valid_epoch;
        if age > kind.max_staleness().as_secs() as i64 {
            warn!(
                source_id = %self.id,
                product = %product,
                valid_epoch = cell.valid_epoch,
                age_secs = age,
                "MRMS grid is stale (valid time older than this product's staleness window); skipped"
            );
            return None;
        }

        let Some(mm_or_mmhr) = cell.value else {
            debug!(
                source_id = %self.id,
                product = %product,
                "MRMS cell is no-coverage / missing this cycle; skipped (not emitted as 0)"
            );
            return None;
        };
        let fields = rain_fields(kind, mm_or_mmhr);
        if fields.is_empty() {
            return None;
        }
        Some(DecodedObservation {
            fields,
            valid_epoch: cell.valid_epoch,
        })
    }
}

/// A decoded MRMS observation ready to emit: the canonical rain fields plus the
/// GRIB's own VALID-time epoch to stamp on the event (NOT wall-clock now).
struct DecodedObservation {
    fields: Vec<(WeatherField, f64)>,
    valid_epoch: i64,
}

/// The single decoded cell over the deployment point plus the message's VALID
/// time. `value` is `None` for a no-coverage / missing (negative) cell; the
/// `valid_epoch` is always set (so the caller can reject a stale grid even when
/// the cell itself is no-coverage).
struct DecodedCell {
    value: Option<f64>,
    valid_epoch: i64,
}

/// Gunzip a gzip member into its decompressed bytes.
fn gunzip(gz: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut decoder = flate2::read::GzDecoder::new(gz);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out)?;
    Ok(out)
}

/// Build the canonical rain field list from a single MRMS cell value, given how
/// the product encodes it. mm (accumulation) or mm/hr (rate) -> inches via the
/// shared units seam. Never called for a missing/negative cell (the caller skips
/// those upstream), but defends anyway: a negative slips through as no fields.
fn rain_fields(kind: RainKind, value: f64) -> Vec<(WeatherField, f64)> {
    if !value.is_finite() || value < MRMS_MIN_VALID {
        return Vec::new();
    }
    use crate::sources::units::to_canonical;
    match kind {
        RainKind::Rate => {
            // mm/hr -> in/hr.
            let in_hr = to_canonical(WeatherField::RainIntensityInHr, value, Some("mm/h"));
            vec![(WeatherField::RainIntensityInHr, in_hr)]
        }
        RainKind::Accumulation { accum_is_hourly } => {
            // mm accumulated -> inches accumulated.
            let inches = to_canonical(WeatherField::RainTodayIn, value, Some("mm"));
            let mut out = vec![(WeatherField::RainTodayIn, inches)];
            if accum_is_hourly {
                // A 1 hour accumulation reads, over that hour, as an in/hr rate,
                // so it also feeds the intensity field (same treatment the NWS
                // adapter gives its last-hour gauge total).
                let in_hr = to_canonical(WeatherField::RainIntensityInHr, value, Some("mm"));
                out.push((WeatherField::RainIntensityInHr, in_hr));
            }
            out
        }
    }
}

/// Decode the first GRIB2 message in `grib` and return the cell nearest
/// `(lat, lon)` together with the message's VALID time. The cell `value` is
/// `None` when that cell is a no-coverage / missing fill (negative) so the
/// caller skips it; the `valid_epoch` is always populated. Errors only on a
/// structurally bad message (no message, no grid, point outside the grid bounds,
/// or a `data()` decode failure, which is what a missing PNG decoder produces).
fn decode_point_value(grib: &[u8], lat: f64, lon: f64) -> anyhow::Result<DecodedCell> {
    let message = gribberish::message::read_messages(grib)
        .next()
        .ok_or_else(|| anyhow::anyhow!("no GRIB message found in MRMS payload"))?;

    let valid_epoch = message_valid_epoch(&message);

    let geom = grid_geometry(&message)?;
    let Some(idx) = nearest_cell_index(&geom, lat, lon) else {
        return Err(anyhow::anyhow!(
            "point ({lat}, {lon}) is outside the MRMS grid bounds"
        ));
    };

    // `data()` is the full decoded field in GRIB scan order (row-major
    // `ny x nx`, longitude fastest), so the cell is `row * nx + col`, which is
    // exactly the linear index `nearest_cell_index` returns. MRMS 2D grids are
    // PNG-packed (data-representation template 5.41), so this is where a missing
    // `png` feature would surface as an Err (every cycle decoded nothing).
    let data = message
        .data()
        .map_err(|e| anyhow::anyhow!("MRMS GRIB data decode failed: {e}"))?;
    let value = *data
        .get(idx)
        .ok_or_else(|| anyhow::anyhow!("MRMS cell index {idx} out of decoded data range"))?;

    let value = if !value.is_finite() || value < MRMS_MIN_VALID {
        None
    } else {
        Some(value)
    };
    Ok(DecodedCell { value, valid_epoch })
}

/// The message's VALID-time epoch (seconds), used to stamp the observation and
/// gate staleness. Prefers `forecast_date()` (the time the grid represents, e.g.
/// the end of a 1 hour QPE window), falls back to `reference_date()` (the model
/// run / analysis time) when that is unavailable, and finally to wall-clock now
/// if the message carries no parseable time at all (a malformed grid still gets
/// a sane stamp rather than the epoch, but such a grid almost always fails the
/// grid/data decode above first).
fn message_valid_epoch(message: &gribberish::message::Message<'_>) -> i64 {
    if let Ok(dt) = message.forecast_date() {
        return dt.timestamp();
    }
    if let Ok(dt) = message.reference_date() {
        return dt.timestamp();
    }
    chrono::Utc::now().timestamp()
}

/// Pull the regular-grid geometry from a decoded GRIB2 message: shape from
/// `grid_dimensions` (returns `(ny, nx)`), start coords + steps from the
/// lat/lng projector's regular axes. Errors for a non-regular (projected) grid,
/// which MRMS never is.
fn grid_geometry(message: &gribberish::message::Message<'_>) -> anyhow::Result<GridGeometry> {
    let (ny, nx) = message
        .grid_dimensions()
        .map_err(|e| anyhow::anyhow!("MRMS grid dimensions read failed: {e}"))?;
    let projector = message
        .latlng_projector()
        .map_err(|e| anyhow::anyhow!("MRMS grid projector read failed: {e}"))?;
    if !projector.is_regular_latlng_grid() {
        return Err(anyhow::anyhow!(
            "MRMS message is not a regular lat/lon grid"
        ));
    }
    // `latlng_start`/`latlng_end` give the first and last coordinate of each
    // axis; the per-cell step is the span divided by (count - 1).
    let (lat_start, lon_start) = projector.latlng_start();
    let (lat_end, lon_end) = projector.latlng_end();
    let dlat = axis_step(lat_start, lat_end, ny);
    let dlon = axis_step(lon_start, lon_end, nx);
    Ok(GridGeometry {
        ny,
        nx,
        lat_start,
        lon_start,
        dlat,
        dlon,
    })
}

/// Per-cell step of a regular axis from its endpoints and point count. Zero for
/// a degenerate single-point axis (which `nearest_cell_index` then rejects).
fn axis_step(start: f64, end: f64, count: usize) -> f64 {
    if count <= 1 {
        0.0
    } else {
        (end - start) / (count - 1) as f64
    }
}

/// Linear index into the row-major `ny x nx` decoded field for the cell nearest
/// `(lat, lon)`, or `None` when the point falls outside the grid bounds (so the
/// caller treats it as no coverage rather than clamping to an edge cell).
///
/// Pure arithmetic off the regular grid: the row is the rounded number of
/// `dlat` steps from `lat_start`, the column the rounded number of `dlon` steps
/// from `lon_start`. The deployment longitude is normalized into the grid's
/// 0..360 convention first (MRMS publishes Lo1 about 230, i.e. negative
/// longitudes wrap to +360). This is the unit-tested core of the cell lookup.
fn nearest_cell_index(geom: &GridGeometry, lat: f64, lon: f64) -> Option<usize> {
    if geom.dlat == 0.0 || geom.dlon == 0.0 {
        return None;
    }

    // Normalize the deployment longitude into the grid's longitude convention.
    // MRMS Lo1 is on 0..360 (about 230.005), so a US longitude like -97.0 must
    // be read as 263.0 to land in range.
    let mut lon_n = lon;
    if geom.lon_start >= 0.0 {
        // Grid uses 0..360; fold a signed [-180, 180) longitude up by 360.
        while lon_n < 0.0 {
            lon_n += 360.0;
        }
        while lon_n >= 360.0 {
            lon_n -= 360.0;
        }
    }

    let row_f = (lat - geom.lat_start) / geom.dlat;
    let col_f = (lon_n - geom.lon_start) / geom.dlon;

    // Reject points more than half a cell beyond either edge: outside coverage,
    // not an edge clamp. `row_f`/`col_f` run 0..count-1 inside the grid.
    let row = nearest_in_range(row_f, geom.ny)?;
    let col = nearest_in_range(col_f, geom.nx)?;
    Some(row * geom.nx + col)
}

/// Round a fractional axis coordinate to the nearest integer cell, returning
/// `None` when it is more than half a cell outside `[0, count - 1]` (so an
/// off-grid point is rejected, not clamped). A coordinate within the outer half
/// cell of an edge rounds onto that edge cell.
fn nearest_in_range(coord: f64, count: usize) -> Option<usize> {
    if count == 0 || !coord.is_finite() {
        return None;
    }
    let max = (count - 1) as f64;
    // More than half a cell beyond either edge is out of coverage, not a clamp.
    if coord < -0.5 || coord > max + 0.5 {
        return None;
    }
    // In range (including the outer half-cell of each edge): round to the
    // nearest cell and clamp the rounding back onto the valid [0, count-1] band
    // (a coordinate like -0.3 rounds to 0; max + 0.4 rounds to the last cell).
    let rounded = coord.round().clamp(0.0, max);
    Some(rounded as usize)
}

#[async_trait]
impl WeatherSource for NoaaMrms {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> SourceCaps {
        let mut fields = HashSet::new();
        // CURRENT rain scalars decoded from the MRMS QPE grid cell over the
        // deployment lat/lon: a live in/hr rate (RainIntensityInHr) and the
        // accumulated total today (RainTodayIn). These are the ONLY fields MRMS
        // emits, so it surfaces in the per-field CURRENT rain picker only.
        fields.insert(WeatherField::RainIntensityInHr);
        fields.insert(WeatherField::RainTodayIn);
        SourceCaps {
            // Radar QPE is observation-grade but it is a NATIONAL grid, not a
            // LAN sensor, so live_current stays false: a real on-yard gauge
            // outranks it in the per-field merge.
            live_current: false,
            hourly_forecast_hours: 0,
            daily_forecast_days: 0,
            radar_tiles: false,
            et0_native: false,
            fields,
        }
    }

    fn priority(&self, field: WeatherField) -> i32 {
        match field {
            // Gauge-corrected radar QPE: the best off-yard rain read short of a
            // local gauge, so above NWS station observations (35) yet below any
            // live LAN gauge (live_current sources default ~80-100). 45 ranks
            // MRMS as the strongest US off-yard rain source without displacing a
            // real on-site sensor.
            WeatherField::RainIntensityInHr | WeatherField::RainTodayIn => 45,
            _ => i32::MIN,
        }
    }

    async fn run(
        self: Arc<Self>,
        bus: SourceBus,
        mut shutdown: ShutdownSignal,
    ) -> anyhow::Result<()> {
        info!(
            source_id = %self.id,
            rate_product = %self.rate_product(),
            accum_product = %self.accum_product(),
            "NOAA MRMS source started (two products per cycle)"
        );
        let mut tick = interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_reachable: Option<bool> = None;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    // Fetch + decode + emit BOTH products this cycle. Each is an
                    // independent product with its own grid, valid time, field,
                    // and staleness window, so each emits a SEPARATE observation
                    // stamped with ITS OWN valid time: the fresh PrecipRate rate
                    // and the lagged hourly accumulation never share a timestamp,
                    // and the merge ages each field on its own clock. A None
                    // decode (no coverage / missing / decode error / a grid past
                    // that product's staleness window) emits NOTHING for that
                    // product: MRMS never claims a dry 0 it did not measure. We
                    // stamp the GRIB's own VALID time (not Utc::now), so a stuck
                    // `.latest` grid cannot read as fresh downstream.
                    let now_epoch = chrono::Utc::now().timestamp();
                    let rate_ok = self
                        .poll_product(&bus, self.rate_product(), now_epoch)
                        .await;
                    let accum_ok = self
                        .poll_product(&bus, self.accum_product(), now_epoch)
                        .await;

                    // Reachable if EITHER product fetched ok (a single product's
                    // transient gap does not mark the whole source unreachable).
                    let reachable = rate_ok || accum_ok;
                    // Belt-and-suspenders reachability freshness: send a
                    // Reachability event on EVERY successful poll (not only on a
                    // state CHANGE), so a stably-reachable MRMS keeps a FRESH
                    // reachability epoch in the bus recorder. The bug this guards:
                    // change-only sends left a stably-reachable source with a stale
                    // last-reachable epoch, which read `offline` in the catalog
                    // (>30 min stale) even though it was fetching fine every few
                    // minutes. The observation-liveness proof in /api/config now
                    // covers this too, but keeping the reachability epoch fresh is
                    // cheap (one bounded-channel send every 3 min) and makes the
                    // reachability surface honest on its own. The false EDGE still
                    // fires once on the transition so a real outage is recorded
                    // promptly without then re-sending `false` every cycle.
                    if reachable || last_reachable != Some(reachable) {
                        let _ = bus.send(SourceEvent::Reachability {
                            source_id: self.id.clone(),
                            reachable,
                        });
                        last_reachable = Some(reachable);
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!(source_id = %self.id, "NOAA MRMS source shutting down");
                        return Ok(());
                    }
                }
            }
        }
    }
}

impl NoaaMrms {
    /// Fetch one MRMS product grid, decode the deployment cell, and (on a real
    /// in-window reading) emit a single `Observation` stamped with that grid's
    /// own valid time. Returns whether the FETCH succeeded (the reachability
    /// signal), independent of whether a value was emitted: a successful fetch
    /// whose cell is no-coverage / missing / stale still counts as reachable
    /// (the server answered; there was simply nothing to emit this cycle).
    async fn poll_product(&self, bus: &SourceBus, product: &str, now_epoch: i64) -> bool {
        match self.fetch_grib(product).await {
            Ok(grib) => {
                if let Some(obs) = self.fields_from_grib(product, &grib, now_epoch) {
                    debug!(
                        source_id = %self.id,
                        product = %product,
                        fields_n = obs.fields.len(),
                        valid_epoch = obs.valid_epoch,
                        "MRMS cell decoded; emitting rain observation"
                    );
                    let _ = bus.send(SourceEvent::Observation {
                        source_id: self.id.clone(),
                        fields: obs.fields,
                        at_epoch: obs.valid_epoch,
                    });
                }
                true
            }
            Err(e) => {
                warn!(source_id = %self.id, product = %product, error = %e, "MRMS GRIB fetch failed");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CONUS-shaped synthetic geometry: a coarse stand-in for the real MRMS
    /// grid (north -> south scan, 0..360 longitudes) so the cell arithmetic is
    /// exercised without a real GRIB. Real MRMS: La1 54.995, Lo1 230.005,
    /// 0.01 deg step, 3500 x 7000. We use a 10x10 cell window at 1 deg spacing
    /// over the same corner convention so the indices stay hand-checkable.
    fn conus_geometry() -> GridGeometry {
        GridGeometry {
            ny: 10,
            nx: 10,
            lat_start: 50.0,
            lon_start: 250.0, // == -110 deg on the signed convention
            dlat: -1.0,       // scans north -> south, like MRMS
            dlon: 1.0,        // scans west -> east
        }
    }

    #[test]
    fn nearest_cell_index_at_grid_origin() {
        let geom = conus_geometry();
        // The start corner (lat_start, lon_start) is row 0, col 0 -> index 0.
        assert_eq!(nearest_cell_index(&geom, 50.0, -110.0), Some(0));
    }

    #[test]
    fn nearest_cell_index_signed_longitude_normalized() {
        let geom = conus_geometry();
        // A signed US longitude (-104 deg == 256 on the grid's 0..360 axis) is
        // 6 columns east of Lo1 (250). Lat 47 is 3 rows south of La1 (50) on the
        // descending axis. Index = row 3 * nx 10 + col 6 = 36.
        assert_eq!(nearest_cell_index(&geom, 47.0, -104.0), Some(36));
        // The same point given as an already-0..360 longitude lands identically.
        assert_eq!(nearest_cell_index(&geom, 47.0, 256.0), Some(36));
    }

    #[test]
    fn nearest_cell_index_rounds_to_closest_cell() {
        let geom = conus_geometry();
        // 46.6 deg is 3.4 rows south of 50 -> rounds to row 3.
        // -106.4 deg (== 253.6) is 3.6 cols east of 250 -> rounds to col 4.
        // Index = 3 * 10 + 4 = 34.
        assert_eq!(nearest_cell_index(&geom, 46.6, -106.4), Some(34));
    }

    #[test]
    fn nearest_cell_index_last_cell() {
        let geom = conus_geometry();
        // Far corner: lat 41 (9 rows south), lon -101 (== 259, 9 cols east).
        // Index = 9 * 10 + 9 = 99 = ny*nx - 1.
        assert_eq!(nearest_cell_index(&geom, 41.0, -101.0), Some(99));
    }

    #[test]
    fn nearest_cell_index_rejects_off_grid_point() {
        let geom = conus_geometry();
        // Well north of La1 (more than half a cell past the top row) -> None,
        // so an out-of-coverage point is skipped, never clamped to an edge cell.
        assert_eq!(nearest_cell_index(&geom, 60.0, -110.0), None);
        // Well east of the last column -> None.
        assert_eq!(nearest_cell_index(&geom, 47.0, -90.0), None);
        // Well west of the first column -> None.
        assert_eq!(nearest_cell_index(&geom, 47.0, -120.0), None);
    }

    #[test]
    fn nearest_cell_index_within_half_cell_of_edge_clamps() {
        let geom = conus_geometry();
        // 50.4 deg is 0.4 cells north of the top row (within half a cell) ->
        // rounds onto row 0, not rejected.
        assert_eq!(nearest_cell_index(&geom, 50.4, -110.0), Some(0));
    }

    #[test]
    fn product_classification() {
        // Default gauge-corrected pass-2 1 hour QPE: hourly accumulation.
        assert_eq!(
            classify_product("MultiSensor_QPE_01H_Pass2"),
            RainKind::Accumulation {
                accum_is_hourly: true
            }
        );
        // A 24 hour QPE: accumulation, but not an hourly intensity.
        assert_eq!(
            classify_product("MultiSensor_QPE_24H_Pass2"),
            RainKind::Accumulation {
                accum_is_hourly: false
            }
        );
        // PrecipRate: instantaneous mm/hr rate.
        assert_eq!(classify_product("PrecipRate"), RainKind::Rate);
    }

    #[test]
    fn hourly_accumulation_emits_both_fields_mm_to_in() {
        // 25.4 mm over the last hour == 1.0 inch accumulation AND a 1.0 in/hr
        // rate over that hour (matches the NWS last-hour-gauge treatment).
        let fields = rain_fields(
            RainKind::Accumulation {
                accum_is_hourly: true,
            },
            25.4,
        );
        let today = fields
            .iter()
            .find(|(f, _)| *f == WeatherField::RainTodayIn)
            .map(|(_, v)| *v);
        let rate = fields
            .iter()
            .find(|(f, _)| *f == WeatherField::RainIntensityInHr)
            .map(|(_, v)| *v);
        assert!((today.unwrap() - 1.0).abs() < 1e-9, "today inches");
        assert!((rate.unwrap() - 1.0).abs() < 1e-9, "in/hr rate");
    }

    #[test]
    fn rate_product_emits_only_intensity_mmhr_to_inhr() {
        // 50.8 mm/hr == 2.0 in/hr, intensity only (no accumulation total).
        let fields = rain_fields(RainKind::Rate, 50.8);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].0, WeatherField::RainIntensityInHr);
        assert!((fields[0].1 - 2.0).abs() < 1e-9);
    }

    #[test]
    fn longer_accumulation_emits_only_total() {
        // A 24 hour QPE emits the accumulation total but NOT an in/hr intensity
        // (the window is not one hour, so the rate reading is not meaningful).
        let fields = rain_fields(
            RainKind::Accumulation {
                accum_is_hourly: false,
            },
            25.4,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].0, WeatherField::RainTodayIn);
        assert!((fields[0].1 - 1.0).abs() < 1e-9);
    }

    #[test]
    fn missing_flag_is_skipped_not_emitted_as_zero() {
        // MRMS encodes no-coverage as -3 and missing as -999. Both must yield
        // NO fields (skip), never a (RainTodayIn, 0.0) that would falsely read
        // as a confirmed dry zero and green-light irrigation.
        for flag in [-3.0, -999.0, f64::NAN, f64::NEG_INFINITY] {
            let acc = rain_fields(
                RainKind::Accumulation {
                    accum_is_hourly: true,
                },
                flag,
            );
            assert!(acc.is_empty(), "accumulation flag {flag} must be skipped");
            let rate = rain_fields(RainKind::Rate, flag);
            assert!(rate.is_empty(), "rate flag {flag} must be skipped");
        }
    }

    #[test]
    fn zero_accumulation_is_a_real_dry_reading() {
        // A real measured 0.0 mm (radar saw the cell, no rain fell) is a VALID
        // reading and must emit, distinct from a negative no-coverage flag.
        let fields = rain_fields(
            RainKind::Accumulation {
                accum_is_hourly: true,
            },
            0.0,
        );
        assert!(!fields.is_empty());
        assert!(fields
            .iter()
            .any(|(f, v)| *f == WeatherField::RainTodayIn && *v == 0.0));
    }

    #[test]
    fn axis_step_handles_descending_and_degenerate() {
        // North -> south MRMS latitude axis: negative step.
        assert!((axis_step(54.995, 20.005, 3500) - (-0.01)).abs() < 1e-6);
        // West -> east longitude axis: positive step.
        assert!((axis_step(230.005, 299.995, 7000) - 0.01).abs() < 1e-6);
        // Single-point axis is degenerate -> zero step (rejected downstream).
        assert_eq!(axis_step(50.0, 50.0, 1), 0.0);
    }

    #[test]
    fn latest_url_uses_product_symlink() {
        let src = NoaaMrms::new(
            "mrms",
            NoaaMrmsConfig::default(),
            Location {
                lat: 39.0,
                lon: -97.0,
                elevation_m: None,
            },
        );
        // Both products resolve to their own `.latest.grib2.gz` symlink: the
        // gauge-corrected hourly accumulation and the instantaneous PrecipRate.
        assert_eq!(
            src.latest_url(src.accum_product()),
            "https://mrms.ncep.noaa.gov/2D/MultiSensor_QPE_01H_Pass2/MRMS_MultiSensor_QPE_01H_Pass2.latest.grib2.gz"
        );
        assert_eq!(
            src.latest_url(src.rate_product()),
            "https://mrms.ncep.noaa.gov/2D/PrecipRate/MRMS_PrecipRate.latest.grib2.gz"
        );
        // The defaults are the two intended products.
        assert_eq!(src.accum_product(), "MultiSensor_QPE_01H_Pass2");
        assert_eq!(src.rate_product(), "PrecipRate");
    }

    #[test]
    fn caps_and_priority_match_radar_qpe_contract() {
        let src = NoaaMrms::new(
            "mrms",
            NoaaMrmsConfig::default(),
            Location {
                lat: 39.0,
                lon: -97.0,
                elevation_m: None,
            },
        );
        let caps = src.capabilities();
        assert!(caps.fields.contains(&WeatherField::RainIntensityInHr));
        assert!(caps.fields.contains(&WeatherField::RainTodayIn));
        // National grid, not a LAN sensor.
        assert!(!caps.live_current);
        // Above NWS station obs (35), below a LAN gauge (~80+).
        assert_eq!(src.priority(WeatherField::RainTodayIn), 45);
        assert_eq!(src.priority(WeatherField::RainIntensityInHr), 45);
        assert_eq!(src.priority(WeatherField::AirTempF), i32::MIN);
    }

    // ---------------------------------------------------------------------
    // REAL-GRIB regression test (the zero-decode guard).
    //
    // The synthetic-geometry unit tests above never call `message.data()`, so a
    // build with the PNG decoder dropped (MRMS 2D grids are data-representation
    // template 5.41 / PNG-packed) stayed green while the source silently decoded
    // NOTHING every cycle. This test decodes a REAL committed MRMS .grib2.gz end
    // to end through the production path (`decode_point_value` -> `data()`), so
    // if the `gribberish` `png` feature is ever dropped again `data()` returns
    // Err and this test fails loudly. The fixture is a single real
    // MultiSensor_QPE_01H_Pass2 grid (~0.5 MB gzipped, the full CONUS PNG grid;
    // no smaller PNG-packed MRMS grid exists) pulled 2026-06-29.
    // ---------------------------------------------------------------------

    /// A committed real MRMS QPE grid (PNG-packed, DRT 5.41), gzip-wrapped on the
    /// wire exactly as NCEP serves it.
    const FIXTURE_GZ: &[u8] = include_bytes!("testdata/mrms_qpe_01h_pass2.grib2.gz");

    /// The fixture's own GRIB VALID time, 2026-06-29T18:00:00Z, as a Unix epoch.
    /// Used to drive the staleness gate deterministically (the fixture is a fixed
    /// past grid, so we pass a `now` near it for the in-window cases).
    const FIXTURE_VALID_EPOCH: i64 = 1_782_756_000;

    /// A known WET cell in this exact grid: 74.6 mm == 2.9370 in over the 1 hour
    /// window, at lat 45.5650, lon -84.6950 (northern Michigan). Found by scanning
    /// the fully decoded field for its maximum; locked in so the assertion also
    /// proves the cell-index arithmetic lands on the right physical location.
    const WET_LAT: f64 = 45.5650;
    const WET_LON: f64 = -84.6950;
    const WET_MM: f64 = 74.6;

    /// A committed real MRMS PrecipRate grid (PNG-packed, DRT 5.41, the
    /// instantaneous radar rain RATE in mm/hr), gzip-wrapped on the wire exactly
    /// as NCEP serves it. Pulled live 2026-06-29, alongside the QPE fixture, so
    /// the RATE decode path is regression-covered end to end through `data()`
    /// (a dropped `png` feature fails here too, not just on the QPE grid).
    const RATE_FIXTURE_GZ: &[u8] = include_bytes!("testdata/mrms_preciprate.grib2.gz");

    /// The PrecipRate fixture's own GRIB VALID time, 2026-06-29T22:02:00Z.
    const RATE_FIXTURE_VALID_EPOCH: i64 = 1_782_770_520;

    /// A known WET cell in the PrecipRate fixture: 148.8 mm/hr == 5.8583 in/hr,
    /// at lat 25.8650, lon -81.0850 (a heavy convective cell over south Florida).
    /// Found by scanning the fully decoded field for its maximum, so the assertion
    /// proves both the PNG decode and that the cell-index arithmetic lands on the
    /// right physical location, on the SAME 0.01 deg CONUS grid as the QPE grid.
    const RATE_WET_LAT: f64 = 25.8650;
    const RATE_WET_LON: f64 = -81.0850;
    const RATE_WET_MMHR: f64 = 148.8;

    #[cfg(feature = "ssr")]
    #[test]
    fn real_grib_decodes_geometry_and_known_wet_cell() {
        let grib = gunzip(FIXTURE_GZ).expect("fixture gunzips");

        // Geometry straight off the real GRIB grid definition. This proves the
        // message parsed and the projector read; the data decode is asserted
        // below. ny=3500/nx=7000 is the 0.01 deg CONUS grid.
        let message = gribberish::message::read_messages(&grib)
            .next()
            .expect("a GRIB message");
        let geom = grid_geometry(&message).expect("regular lat/lon geometry");
        assert_eq!(geom.ny, 3500, "MRMS CONUS rows");
        assert_eq!(geom.nx, 7000, "MRMS CONUS cols");
        // Descending latitude (north -> south scan): La1 about 54.995, step < 0.
        assert!(
            geom.lat_start > 54.0 && geom.lat_start < 55.0,
            "La1 ~54.995"
        );
        assert!(geom.dlat < 0.0, "latitude scans north -> south");
        assert!((geom.dlat - (-0.01)).abs() < 1e-4, "0.01 deg lat step");
        // 0..360 longitude convention (Lo1 about 230.005), step > 0 west -> east.
        assert!(
            geom.lon_start > 200.0 && geom.lon_start < 360.0,
            "Lo1 on 0..360 (~230.005)"
        );
        assert!(geom.dlon > 0.0, "longitude scans west -> east");
        assert!((geom.dlon - 0.01).abs() < 1e-4, "0.01 deg lon step");

        // THE CRUX: the full production decode path, which calls `message.data()`
        // (PNG-unpack of the DRT-5.41 field). A dropped `png` feature makes this
        // Err and the test fails here. The known wet cell must come back at its
        // measured 74.6 mm.
        let cell = decode_point_value(&grib, WET_LAT, WET_LON)
            .expect("decode_point_value Ok (data() decoded the PNG-packed field)");
        let value = cell
            .value
            .expect("the known wet cell is a valid (non-negative) reading");
        assert!(
            (value - WET_MM).abs() < 0.05,
            "known wet cell decodes to {WET_MM} mm, got {value} mm"
        );

        // The valid epoch threads out of the real message (forecast_date).
        assert_eq!(
            cell.valid_epoch, FIXTURE_VALID_EPOCH,
            "valid time stamped from the GRIB's own forecast_date"
        );
    }

    #[cfg(feature = "ssr")]
    #[test]
    fn real_grib_fields_from_grib_emits_inches_for_wet_cell() {
        let grib = gunzip(FIXTURE_GZ).expect("fixture gunzips");
        let src = NoaaMrms::new(
            "mrms",
            NoaaMrmsConfig::default(),
            Location {
                lat: WET_LAT,
                lon: WET_LON,
                elevation_m: None,
            },
        );

        // Pretend "now" is one minute after the grid's valid time so the
        // staleness gate passes and we exercise the full emit path end to end.
        let now = FIXTURE_VALID_EPOCH + 60;
        let obs = src
            .fields_from_grib(src.accum_product(), &grib, now)
            .expect("a fresh wet grid emits an observation");
        // Stamped with the GRIB valid time, NOT `now`.
        assert_eq!(obs.valid_epoch, FIXTURE_VALID_EPOCH);

        // 74.6 mm over the hour == 2.9370 in: emitted as BOTH the accumulation
        // total and the in/hr intensity (hourly QPE), proving the mm->in seam ran
        // on a real decoded value.
        let today = obs
            .fields
            .iter()
            .find(|(f, _)| *f == WeatherField::RainTodayIn)
            .map(|(_, v)| *v)
            .expect("RainTodayIn emitted");
        let rate = obs
            .fields
            .iter()
            .find(|(f, _)| *f == WeatherField::RainIntensityInHr)
            .map(|(_, v)| *v)
            .expect("RainIntensityInHr emitted");
        let expect_in = WET_MM / 25.4;
        assert!(
            (today - expect_in).abs() < 1e-3,
            "RainTodayIn ~{expect_in} in, got {today}"
        );
        assert!(
            (rate - expect_in).abs() < 1e-3,
            "RainIntensityInHr ~{expect_in} in/hr, got {rate}"
        );
    }

    #[cfg(feature = "ssr")]
    #[test]
    fn real_grib_stale_accumulation_grid_is_skipped_not_emitted() {
        let grib = gunzip(FIXTURE_GZ).expect("fixture gunzips");
        let src = NoaaMrms::new(
            "mrms",
            NoaaMrmsConfig::default(),
            Location {
                lat: WET_LAT,
                lon: WET_LON,
                elevation_m: None,
            },
        );
        // "now" is well past the ACCUMULATION staleness window (3 hr) after the
        // grid's valid time: a stuck `.latest` publish must emit NOTHING, never a
        // stale-looking reading that would read fresh to the freshness gate.
        let now = FIXTURE_VALID_EPOCH + ACCUM_MAX_STALENESS.as_secs() as i64 + 600;
        assert!(
            src.fields_from_grib(src.accum_product(), &grib, now)
                .is_none(),
            "a grid older than the accumulation staleness window is dropped"
        );
        // And inside the 3 hr accumulation window it DOES emit, even at the ~1 to
        // 1.5 hr lag that is normal for a gauge-corrected hourly grid (the lag is
        // expected and the reading is still decision-useful).
        let lagged_now = FIXTURE_VALID_EPOCH + 90 * 60;
        assert!(
            src.fields_from_grib(src.accum_product(), &grib, lagged_now)
                .is_some(),
            "a 90 min lagged hourly accumulation is still inside its 3 hr window"
        );
    }

    #[cfg(feature = "ssr")]
    #[test]
    fn rate_product_staleness_window_is_tight() {
        // The RATE product (PrecipRate) is reused against the QPE fixture purely
        // to exercise the per-product staleness threshold: classified as Rate, a
        // grid 90 min past its valid time is OUTSIDE the 45 min rate window and
        // dropped, even though the same grid is inside the 3 hr accumulation
        // window. This proves the two products age on their own clocks.
        let grib = gunzip(FIXTURE_GZ).expect("fixture gunzips");
        let src = NoaaMrms::new(
            "mrms",
            NoaaMrmsConfig::default(),
            Location {
                lat: WET_LAT,
                lon: WET_LON,
                elevation_m: None,
            },
        );
        let now_90m = FIXTURE_VALID_EPOCH + 90 * 60;
        assert!(
            src.fields_from_grib(src.rate_product(), &grib, now_90m)
                .is_none(),
            "a 90 min old grid is past the 45 min rate window"
        );
        // Inside the 45 min rate window the SAME grid decodes and emits a rate.
        let now_30m = FIXTURE_VALID_EPOCH + 30 * 60;
        let obs = src
            .fields_from_grib(src.rate_product(), &grib, now_30m)
            .expect("a 30 min old grid is inside the 45 min rate window");
        // Read as a RATE it emits ONLY RainIntensityInHr (no accumulation total),
        // converting the cell mm value through the mm/hr -> in/hr seam.
        assert_eq!(obs.fields.len(), 1, "rate product emits intensity only");
        assert_eq!(obs.fields[0].0, WeatherField::RainIntensityInHr);
        assert!((obs.fields[0].1 - WET_MM / 25.4).abs() < 1e-3);
        assert_eq!(obs.valid_epoch, FIXTURE_VALID_EPOCH);
    }

    #[cfg(feature = "ssr")]
    #[test]
    fn real_preciprate_grib_decodes_and_emits_in_hr_rate() {
        // THE RATE-PRODUCT CRUX: decode a REAL committed PrecipRate grid end to
        // end through the production path (`fields_from_grib` -> `decode_point_value`
        // -> `data()`, the PNG-unpack of the DRT-5.41 field). A dropped `png`
        // feature makes `data()` Err and this fails loudly, exactly like the QPE
        // regression test, so the new RATE product is covered too.
        let grib = gunzip(RATE_FIXTURE_GZ).expect("rate fixture gunzips");

        // Same 0.01 deg CONUS grid as the QPE product.
        let message = gribberish::message::read_messages(&grib)
            .next()
            .expect("a GRIB message");
        let geom = grid_geometry(&message).expect("regular lat/lon geometry");
        assert_eq!(geom.ny, 3500, "MRMS CONUS rows");
        assert_eq!(geom.nx, 7000, "MRMS CONUS cols");

        let src = NoaaMrms::new(
            "mrms",
            NoaaMrmsConfig::default(),
            Location {
                lat: RATE_WET_LAT,
                lon: RATE_WET_LON,
                elevation_m: None,
            },
        );
        // "now" one minute after the rate grid's valid time: well inside the
        // 45 min rate window, so the full emit path runs.
        let now = RATE_FIXTURE_VALID_EPOCH + 60;
        let obs = src
            .fields_from_grib(src.rate_product(), &grib, now)
            .expect("a fresh PrecipRate grid emits an observation");
        // Stamped with the GRIB valid time, NOT `now`.
        assert_eq!(obs.valid_epoch, RATE_FIXTURE_VALID_EPOCH);

        // 148.8 mm/hr == 5.8583 in/hr: a RATE emits ONLY the intensity field
        // (no accumulation total), proving the mm/hr -> in/hr seam ran on a real
        // decoded PrecipRate cell.
        assert_eq!(obs.fields.len(), 1, "rate emits intensity only");
        assert_eq!(obs.fields[0].0, WeatherField::RainIntensityInHr);
        let expect_in_hr = RATE_WET_MMHR / 25.4;
        assert!(
            (obs.fields[0].1 - expect_in_hr).abs() < 1e-3,
            "RainIntensityInHr ~{expect_in_hr} in/hr, got {}",
            obs.fields[0].1
        );
    }

    #[test]
    fn per_product_staleness_windows_match_contract() {
        // The two products age on their own clocks: the instantaneous rate
        // tolerates only a short lag (~45 min, since a product refreshed every
        // couple minutes should never be older), while the gauge-corrected
        // hourly accumulation tolerates a wide window (~3 hr, since it publishes
        // ~80 min late by design and is still decision-useful at ~1 to 1.5 hr).
        assert_eq!(RATE_MAX_STALENESS, Duration::from_secs(45 * 60));
        assert_eq!(ACCUM_MAX_STALENESS, Duration::from_secs(3 * 60 * 60));
        // The fresh rate must have a tighter window than the lagged accumulation
        // (a const comparison, so const-asserted to dodge a clippy const-value lint).
        const _: () = assert!(RATE_MAX_STALENESS.as_secs() < ACCUM_MAX_STALENESS.as_secs());
        // RainKind::max_staleness selects the right window per classification.
        assert_eq!(RainKind::Rate.max_staleness(), RATE_MAX_STALENESS);
        assert_eq!(
            RainKind::Accumulation {
                accum_is_hourly: true
            }
            .max_staleness(),
            ACCUM_MAX_STALENESS
        );
        assert_eq!(
            RainKind::Accumulation {
                accum_is_hourly: false
            }
            .max_staleness(),
            ACCUM_MAX_STALENESS
        );
        // And the default config wires PrecipRate to Rate, the QPE to Accumulation.
        assert_eq!(classify_product("PrecipRate"), RainKind::Rate);
        assert!(matches!(
            classify_product("MultiSensor_QPE_01H_Pass2"),
            RainKind::Accumulation { .. }
        ));
    }

    // ---------------------------------------------------------------------
    // Bounded-retry regression (the dropped-body robustness fix).
    //
    // NCEP intermittently drops the PrecipRate response body mid-transfer
    // (reqwest "error decoding response body"). `fetch_grib` must retry the GET
    // up to FETCH_MAX_RETRIES times so a single dropped body recovers within the
    // poll cycle instead of failing it (which would flip the source unreachable
    // and stall the live rate). These tests drive `fetch_gz_with_retry` against a
    // local one-shot TCP server that drops the body on the first attempts then
    // serves a valid gzip body, with no MRMS_BASE / network dependency.
    // ---------------------------------------------------------------------

    /// gzip-wrap an arbitrary payload exactly as NCEP serves the `.grib2.gz`
    /// (so `gunzip` downstream round-trips it). Only the retry/transport is under
    /// test here, so the payload is opaque bytes, not a real GRIB.
    fn gzip_bytes(payload: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(payload).unwrap();
        enc.finish().unwrap()
    }

    /// Spawn a local HTTP/1.1 server that, for the first `drop_n` requests, sends
    /// a response whose `Content-Length` overstates the bytes actually written and
    /// then closes the socket (so reqwest's body read fails with "error decoding
    /// response body", the exact NCEP drop), and on every subsequent request
    /// serves `good_body` in full. Returns the bound URL. The server lives on its
    /// own task for the test's duration.
    async fn spawn_flaky_server(drop_n: usize, good_body: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut served = 0usize;
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                // Drain the request line/headers enough that the client's write
                // side is satisfied (we don't parse; MRMS is a simple GET).
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                if served < drop_n {
                    // Promise 64 bytes, send 8, then hang up: a truncated body the
                    // client surfaces as a decode error.
                    let head = b"HTTP/1.1 200 OK\r\nContent-Length: 64\r\n\r\n";
                    let _ = sock.write_all(head).await;
                    let _ = sock.write_all(b"trunc!!!").await;
                    let _ = sock.shutdown().await;
                } else {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                        good_body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&good_body).await;
                    let _ = sock.shutdown().await;
                }
                served += 1;
            }
        });
        format!("http://{addr}/MRMS_PrecipRate.latest.grib2.gz")
    }

    fn test_source() -> NoaaMrms {
        NoaaMrms::new(
            "mrms",
            NoaaMrmsConfig::default(),
            Location {
                lat: 39.0,
                lon: -97.0,
                elevation_m: None,
            },
        )
    }

    #[tokio::test]
    async fn fetch_retries_past_a_single_dropped_body_then_succeeds() {
        // One dropped body (within the 2-retry budget): the fetch must recover and
        // return the full body on the retry, so a single NCEP drop never fails the
        // poll cycle. The recovered gz must gunzip back to the original payload.
        let payload = b"a small opaque grib-ish payload";
        let url = spawn_flaky_server(1, gzip_bytes(payload)).await;
        let src = test_source();
        let gz = src
            .fetch_gz_with_retry(&url, "PrecipRate")
            .await
            .expect("a single dropped body is retried and recovered");
        assert_eq!(
            gunzip(&gz).expect("recovered body gunzips"),
            payload,
            "the retried fetch returns the full, valid body"
        );
    }

    #[tokio::test]
    async fn fetch_recovers_at_the_retry_budget_boundary() {
        // Exactly FETCH_MAX_RETRIES (2) drops, then success: the third attempt
        // (last allowed) recovers, proving the budget is inclusive of the final
        // retry, not off-by-one.
        let payload = b"boundary payload";
        let url = spawn_flaky_server(FETCH_MAX_RETRIES as usize, gzip_bytes(payload)).await;
        let src = test_source();
        let gz = src
            .fetch_gz_with_retry(&url, "PrecipRate")
            .await
            .expect("recovers on the final allowed attempt");
        assert_eq!(gunzip(&gz).expect("gunzips"), payload);
    }

    #[tokio::test]
    async fn fetch_gives_up_after_exhausting_the_retry_budget() {
        // More drops than the budget (3 > 2 retries): every attempt fails, so the
        // fetch surfaces the error and the caller keeps its existing emit-nothing
        // behavior (no fabricated reading, no infinite loop).
        let url =
            spawn_flaky_server(FETCH_MAX_RETRIES as usize + 1, gzip_bytes(b"never reached")).await;
        let src = test_source();
        let err = src
            .fetch_gz_with_retry(&url, "PrecipRate")
            .await
            .expect_err("exhausting the retry budget surfaces an error");
        // It is the transient body/transport error, not a fabricated success.
        let msg = format!("{err:#}");
        assert!(
            !msg.is_empty(),
            "the final failure carries the underlying fetch error"
        );
    }
}
