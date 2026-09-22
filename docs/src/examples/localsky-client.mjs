#!/usr/bin/env node
// Read a LocalSky forecast window. Node.js 22+; no packages required.
import { pathToFileURL } from "node:url";

export class LocalSkyClient {
  constructor(url, token) {
    const parsed = new URL(url);
    if (!["http:", "https:"].includes(parsed.protocol) || parsed.username ||
        parsed.password || parsed.search || parsed.hash) {
      throw new Error("Use an HTTP(S) instance URL without credentials, query, or fragment.");
    }
    this.base = url.replace(/\/$/, "");
    this.token = token;
  }

  async get(path, query = {}) {
    if (!["/info", "/forecast/snapshot", "/forecast/window"].includes(path)) {
      throw new Error("Unsupported read operation.");
    }
    const url = new URL(this.base + "/api/v1" + path);
    url.search = new URLSearchParams(query).toString();
    const headers = { Accept: "application/json" };
    if (this.token) headers.Authorization = "Bearer " + this.token;
    let response;
    try {
      response = await fetch(url, {
        headers, redirect: "error", signal: AbortSignal.timeout(15000),
      });
    } catch {
      throw new Error(`GET ${path}: connection, redirect, or timeout failure. Check address, network, and TLS.`);
    }
    const chunks = [];
    let size = 0;
    const reader = response.body.getReader();
    try {
      while (true) {
        const { value, done } = await reader.read();
        if (done) break;
        size += value.length;
        if (size > 4 * 1024 * 1024) {
          await reader.cancel();
          throw new Error("Response exceeds the 4 MiB client limit.");
        }
        chunks.push(Buffer.from(value));
      }
    } finally {
      reader.releaseLock();
    }
    let payload;
    try { payload = JSON.parse(Buffer.concat(chunks).toString("utf8")); }
    catch {
      throw new Error(`GET ${path}: HTTP ${response.status}; response is not valid JSON.`);
    }
    if (!response.ok) {
      const code = payload?.code ?? payload?.diagnostic?.failure?.code ?? "unavailable";
      const request = response.headers.get("X-LocalSky-Request-Id") ?? "unavailable";
      throw new Error(`GET ${path}: HTTP ${response.status}; code=${code}; request_id=${request}`);
    }
    return payload;
  }

  async forecast(hours = 3) {
    if (!Number.isInteger(hours) || hours < 1 || hours > 48) {
      throw new Error("hours must be an integer from 1 to 48.");
    }
    const info = await this.get("/info");
    const match = /^2\.(\d+)\.\d+$/.exec(info?.api_version ?? "");
    if (info?.service !== "localsky" || !match || Number(match[1]) < 3) {
      throw new Error("This example requires LocalSky API 2.3.0 through 2.x.");
    }
    const snapshot = await this.get("/forecast/snapshot");
    if (!Array.isArray(snapshot?.hourly)) throw new Error("Forecast response is missing its hourly array.");
    const now = Math.floor(Date.now() / 1000);
    const stamps = [...new Set((snapshot.hourly ?? [])
      .map(row => row?.time_epoch)
      .filter(stamp => Number.isInteger(stamp) && stamp + 3600 > now))]
      .sort((a, b) => a - b);
    if (stamps.length < hours) throw new Error("Not enough forecast rows for this window.");
    const window = await this.get("/forecast/window", {
      track: "merged", from: stamps[0], to: stamps[0] + (hours - 1) * 3600,
    });
    if (!window || typeof window !== "object" || Array.isArray(window)) throw new Error("Forecast window response is not an object.");
    return window;
  }
}

async function main() {
  const args = process.argv.slice(2);
  let hours = 3;
  let maxAge = 3600;
  for (let i = 0; i < args.length; i += 2) {
    const value = Number(args[i + 1]);
    if (args[i] === "--hours") hours = value;
    else if (args[i] === "--max-age-seconds") maxAge = value;
    else throw new Error("Options: --hours 1..48 --max-age-seconds N");
  }
  if (!Number.isInteger(hours) || hours < 1 || hours > 48 ||
      !Number.isInteger(maxAge) || maxAge < 0) {
    throw new Error("hours must be 1..48; max-age-seconds must be nonnegative.");
  }
  const client = new LocalSkyClient(
    process.env.LOCALSKY_URL ?? "http://localhost:8090", process.env.LOCALSKY_TOKEN);
  const forecast = await client.forecast(hours);
  const usable = forecast.complete === true && forecast.precip_sum_in != null &&
    typeof forecast.age_s === "number" && forecast.age_s >= 0 && forecast.age_s <= maxAge;
  console.log(JSON.stringify({ usable_for_rain_summary: usable, forecast }, null, 2));
  process.exitCode = usable ? 0 : 2;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
}
